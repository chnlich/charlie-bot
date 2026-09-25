"""Preview-relay tests: /ws/voice/{session_id} against one registered fake backend.

The relay is driven at the handler level with an ASGI-shaped socket double, and
at the server boundary for its registration and its auth — the same
``_check_ws_auth`` the session socket uses. The relay code names no backend: a
fake registered through ``register_transcription_backend`` is all it takes.
"""

from __future__ import annotations

import asyncio
import json

import pytest
from conftest import stub_credentials
from fastapi import WebSocketDisconnect
from fastapi.testclient import TestClient

import server
from src.agents.transcription import registry
from src.agents.transcription.base import TranscriptEvent, TranscriptionBackend, TranscriptionRejected
from src.api import voice
from src.core.config import CharlieBotConfig


class _FakePreviewSocket:
  """ASGI-shaped WebSocket double: scripted incoming messages, recorded outgoing frames.

  Like the real socket, receive() yields to the event loop between messages so
  the relay's streamer task gets its turns, and raising after the disconnect
  message turns any accidental extra receive into a loud test failure.
  """

  def __init__(self, incoming: list[dict]) -> None:
    self._incoming = list(incoming)
    self.sent: list[dict] = []
    self.accepted = False
    self.close_code: int | None = None

  async def accept(self) -> None:
    self.accepted = True

  async def receive(self) -> dict:
    await asyncio.sleep(0)
    if not self._incoming:
      return {"type": "websocket.disconnect", "code": 1000}
    message = self._incoming.pop(0)
    if message["type"] == "websocket.disconnect":
      self._incoming = None  # a further receive is a relay bug: fail loud below
    return message

  async def send_json(self, payload: dict) -> None:
    if self._incoming is None:
      raise RuntimeError("send after the browser disconnected")
    self.sent.append(payload)

  async def close(self, code: int = 1000) -> None:
    self.close_code = code


class _RelayFakeBackend(TranscriptionBackend):
  """One fake live backend: one partial per chunk, one final, closable."""

  id = "fake"
  label = "Fake Relay Backend"
  live_partials = True

  def __init__(self, cfg: CharlieBotConfig, *, final_text: str = "fake final") -> None:
    self.cfg = cfg
    self.final_text = final_text
    self.audio_chunks: list[bytes] = []
    self.vocabulary: list[str] | None = None
    self.languages: list[str] | None = None
    self.iterator_closed = False

  async def transcribe(self, audio, *, vocabulary, languages):
    self.vocabulary = list(vocabulary)
    self.languages = list(languages)
    try:
      async for chunk in audio:
        self.audio_chunks.append(chunk)
        yield TranscriptEvent(kind="partial", text=f"partial {len(self.audio_chunks)}")
      yield TranscriptEvent(kind="final", text=self.final_text)
    finally:
      self.iterator_closed = True


class _RejectingBackend(_RelayFakeBackend):
  """A backend that refuses the session after the first chunk arrives."""

  async def transcribe(self, audio, *, vocabulary, languages):
    try:
      async for _chunk in audio:
        raise TranscriptionRejected("needs fake.key")
      yield TranscriptEvent(kind="final", text="")  # pragma: no cover - never reached
    finally:
      self.iterator_closed = True


class _LogRecorder:
  """The log double: records (event, fields) pairs instead of rendering."""

  def __init__(self) -> None:
    self.events: list[tuple[str, dict]] = []

  def warning(self, event: str, **fields: object) -> None:
    self.events.append((event, fields))

  def debug(self, event: str, **fields: object) -> None:
    self.events.append((event, fields))

  def info(self, event: str, **fields: object) -> None:
    self.events.append((event, fields))

  def exception(self, event: str, **fields: object) -> None:
    self.events.append((event, fields))


def _connect_message() -> dict:
  return {"type": "websocket.connect"}


def _frame(data: bytes) -> dict:
  return {"type": "websocket.receive", "bytes": data}


def _end_message() -> dict:
  return {"type": "websocket.receive", "text": json.dumps({"type": "end"})}


def _relay_env(
    monkeypatch: pytest.MonkeyPatch,
    backend_cls: type[_RelayFakeBackend] = _RelayFakeBackend,
    incoming: list[dict] | None = None,
) -> tuple[CharlieBotConfig, _RelayFakeBackend, _FakePreviewSocket]:
  """One cfg with vocabulary/languages, one registered fake backend, one socket."""
  monkeypatch.setattr(registry, "_FACTORIES", dict(registry._FACTORIES))
  cfg = CharlieBotConfig(
      backends={"options": []},
      voice={
          "vocabulary": ["CharlieBot"],
          "languages": ["zh", "en"]
      },
  )
  backend = backend_cls(cfg)
  monkeypatch.setattr(voice, "get_config", lambda: cfg)
  monkeypatch.setattr(registry, "_FACTORIES", dict(registry._FACTORIES))
  registry.register_transcription_backend("fake", lambda _cfg, **_kwargs: backend)
  socket = _FakePreviewSocket(incoming or [])
  return cfg, backend, socket


@pytest.mark.asyncio
async def test_relay_forwards_partials_and_final_and_feeds_the_registry_backend(
    monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, backend, socket = _relay_env(
      monkeypatch, incoming=[_connect_message(),
                             _frame(b"\x01\x02"),
                             _frame(b"\x03\x04"),
                             _end_message()])

  await voice.voice_preview_relay(socket, "session-a", "fake")

  # The queue is the audio iterator: frames in capture order, exhausted at end.
  assert backend.audio_chunks == [b"\x01\x02", b"\x03\x04"]
  assert backend.vocabulary == cfg.voice.vocabulary
  assert backend.languages == cfg.voice.languages
  assert socket.sent == [
      {
          "type": "partial",
          "text": "partial 1"
      },
      {
          "type": "partial",
          "text": "partial 2"
      },
      {
          "type": "final",
          "text": "fake final"
      },
  ]
  assert socket.accepted and socket.close_code == 1000
  assert backend.iterator_closed  # a normal end closes the backend iterator too


@pytest.mark.asyncio
async def test_relay_refuses_an_unknown_id_with_one_error_and_a_close(monkeypatch: pytest.MonkeyPatch) -> None:
  _, backend, socket = _relay_env(monkeypatch, incoming=[_connect_message()])

  await voice.voice_preview_relay(socket, "session-a", "nosuch")

  assert socket.sent == [
      {
          "type": "error",
          "message": "unknown transcription backend 'nosuch'; known: local, gemini, muse, fake"
      }
  ]
  assert socket.close_code == 1000
  assert backend.audio_chunks == []  # no backend was ever driven


@pytest.mark.asyncio
async def test_relay_refuses_an_unavailable_backend_naming_the_key(monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, backend, socket = _relay_env(monkeypatch, incoming=[_connect_message()])

  class _LockedBackend(_RelayFakeBackend):

    def unavailable_reason(self) -> str | None:
      return "needs fake.credential_key"

  backend.__class__ = _LockedBackend  # the reason rides the registered instance
  await voice.voice_preview_relay(socket, "session-a", "fake")

  assert socket.sent == [{"type": "error", "message": "voice backend 'fake' is unavailable: needs fake.credential_key"}]
  assert socket.close_code == 1000
  assert backend.audio_chunks == []  # refused before any audio flowed


@pytest.mark.asyncio
async def test_relay_refuses_a_backend_without_live_partials(monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, backend, socket = _relay_env(monkeypatch, incoming=[_connect_message()])
  backend.live_partials = False

  await voice.voice_preview_relay(socket, "session-a", "fake")

  assert socket.sent == [{"type": "error", "message": "voice backend 'fake' does not stream live partials"}]
  assert socket.close_code == 1000
  assert backend.audio_chunks == []


@pytest.mark.asyncio
async def test_relay_queue_overflow_ends_the_preview_with_an_error_and_keeps_draining(
    monkeypatch: pytest.MonkeyPatch) -> None:
  oversized = b"\x00\x00" * 500_000  # one frame past the 30 s budget (960 000 bytes)
  _, backend, socket = _relay_env(
      monkeypatch, incoming=[_connect_message(),
                             _frame(oversized),
                             _frame(b"\x09\x09"),
                             _frame(b"\x0a\x0a")])

  await voice.voice_preview_relay(socket, "session-a", "fake")

  assert len(socket.sent) == 1
  assert socket.sent[0]["type"] == "error"
  assert "overflowed" in socket.sent[0]["message"]
  assert backend.audio_chunks == []  # the preview ended before the backend saw anything
  assert socket.close_code == 1000  # drained until the browser closed, then closed


@pytest.mark.asyncio
async def test_a_protocol_violation_ends_the_preview_with_an_error(monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _backend, socket = _relay_env(
      monkeypatch,
      incoming=[_connect_message(),
                _frame(b"\x01\x02"), {
                    "type": "websocket.receive",
                    "text": "not json"
                }])

  await voice.voice_preview_relay(socket, "session-a", "fake")

  assert [frame["type"] for frame in socket.sent] == ["partial", "error"]
  assert "malformed preview control frame" in socket.sent[-1]["message"]
  assert socket.close_code == 1000


@pytest.mark.asyncio
async def test_a_backend_rejection_becomes_one_error_and_the_failure_log_line(monkeypatch: pytest.MonkeyPatch) -> None:
  recorder = _LogRecorder()
  monkeypatch.setattr(voice, "log", recorder)
  _cfg, backend, socket = _relay_env(
      monkeypatch,
      backend_cls=_RejectingBackend,
      incoming=[_connect_message(), _frame(b"\x01\x02"), _frame(b"\x03\x04")],
  )

  await voice.voice_preview_relay(socket, "session-a", "fake")

  # One error carrying the reason (the key's name, never its value), one log
  # line with the backend id — and nothing else on the wire.
  assert socket.sent == [{"type": "error", "message": "needs fake.key"}]
  failures = [fields for name, fields in recorder.events if name == "voice_preview_failed"]
  assert len(failures) == 1
  fields = failures[0]
  assert fields["backend"] == "fake"
  assert fields["session_id"] == "session-a"
  assert fields["reason"] == "needs fake.key"
  assert backend.iterator_closed


@pytest.mark.asyncio
async def test_a_browser_disconnect_closes_the_backend_iterator(monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, backend, socket = _relay_env(
      monkeypatch,
      incoming=[
          _connect_message(),
          _frame(b"\x01\x02"),
          _frame(b"\x03\x04"), {
              "type": "websocket.disconnect",
              "code": 1000
          }
      ],
  )

  await voice.voice_preview_relay(socket, "session-a", "fake")

  assert backend.iterator_closed  # the backend's own connection closes with it
  assert [frame["type"] for frame in socket.sent] == ["partial", "partial"]  # no error: nobody to hear one
  assert socket.close_code is None  # the browser closed first


# --- The server boundary -----------------------------------------------------


def test_the_relay_is_registered_next_to_the_session_socket() -> None:
  routes = [route for route in server.app.routes if getattr(route, "path", None) == "/ws/voice/{session_id}"]
  assert len(routes) == 1
  assert routes[0].endpoint.__name__ == "voice_preview_websocket"


def test_unauthenticated_preview_connections_are_rejected_like_the_session_socket() -> None:
  stub_credentials({"charliebot": {"access_key": "secret-key"}})
  client = TestClient(server.app)

  with pytest.raises(WebSocketDisconnect) as exc_info, client.websocket_connect("/ws/voice/session-a?backend=local"):
    pass  # pragma: no cover - the handshake never completes

  assert exc_info.value.code == 4401


def test_the_registered_relay_refuses_an_unknown_backend_through_the_real_app() -> None:
  stub_credentials({"charliebot": {"access_key": ""}})  # auth off: the refusal is the subject
  client = TestClient(server.app)

  with client.websocket_connect("/ws/voice/session-a?backend=nosuch") as ws:
    reply = ws.receive_json()

  assert reply["type"] == "error"
  assert "unknown transcription backend 'nosuch'" in reply["message"]
