"""Gemini transcription backend tests against a loopback fake Live API server.

The wire shapes are the ones the 2026-09-24 live probe measured working: setup
then setupComplete, a client-delimited turn (activityStart before the first
chunk, activityEnd after the last, never audioStreamEnd), interims while audio
flows, one final after activityEnd. No network beyond 127.0.0.1.
"""

from __future__ import annotations

import asyncio
import base64
import json
from collections.abc import AsyncIterator
from pathlib import Path

import pytest
from websockets.asyncio.server import serve
from websockets.exceptions import ConnectionClosed

from src.agents.transcription.base import TranscriptEvent, TranscriptionRejected
from src.agents.transcription.gemini import GeminiTranscriptionBackend
from src.core.config import CharlieBotConfig
from src.core.credentials import Credentials


@pytest.fixture
def gemini_credentials(monkeypatch: pytest.MonkeyPatch) -> Credentials:
  """A credentials source with the gemini key set; the value never reaches an assertion."""
  credentials = Credentials(path=Path("credentials.yaml"), sections={"gemini": {"api_key": "test-key"}})
  monkeypatch.setattr("src.agents.transcription.gemini.get_credentials", lambda: credentials)
  return credentials


def _serve(handler):
  return serve(handler, "127.0.0.1", 0)


def _backend(port: int) -> GeminiTranscriptionBackend:
  return GeminiTranscriptionBackend(CharlieBotConfig(), endpoint_url=f"ws://127.0.0.1:{port}")


async def _collect(backend: GeminiTranscriptionBackend, chunks: list[bytes], **kwargs) -> list[TranscriptEvent]:

  async def feed() -> AsyncIterator[bytes]:
    for chunk in chunks:
      yield chunk

  return [event async for event in backend.transcribe(feed(), **kwargs)]


@pytest.mark.asyncio
async def test_setup_frame_turn_delimiting_and_final(gemini_credentials) -> None:
  observed: dict = {"messages": []}

  async def handler(socket) -> None:
    async for raw in socket:
      message = json.loads(raw)
      observed["messages"].append(message)
      if "setup" in message:
        await socket.send(json.dumps({"setupComplete": {}}))
      elif "audio" in message.get("realtimeInput", {}) and len(observed["messages"]) == 3:
        await socket.send(json.dumps({"serverContent": {"interimInputTranscription": {"text": "你 好"}}}))
      elif "activityEnd" in message.get("realtimeInput", {}):
        await socket.send(
            json.dumps({"serverContent": {
                "inputTranscription": {
                    "text": "final text"
                },
                "generationComplete": True,
            }}))
        await socket.close()

  chunks = [b"\x00\x01" * 2048, b"\x02\x03" * 2048]
  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    events = await _collect(_backend(port), chunks, vocabulary=["CharlieBot"], languages=["zh", "en"])

  setup = observed["messages"][0]["setup"]
  assert setup["model"] == "models/gemini-3.5-transcribe-live"
  assert setup["generationConfig"] == {"responseModalities": ["TEXT"]}
  assert setup["inputAudioTranscription"] == {
      "languageCodes": ["cmn-Hans-CN", "en-US"],
      "customVocabulary": ["CharlieBot"],
      "mode": "VERBATIM",
  }
  assert setup["realtimeInputConfig"] == {"automaticActivityDetection": {"disabled": True}}

  # activityStart precedes the audio, activityEnd follows it, and audioStreamEnd
  # is never sent (the probe: nothing arrives after it).
  turns = [next(iter(message["realtimeInput"])) for message in observed["messages"][1:]]
  assert turns == ["activityStart", "audio", "audio", "activityEnd"]
  assert not any("audioStreamEnd" in message.get("realtimeInput", {}) for message in observed["messages"])

  audio = observed["messages"][2]["realtimeInput"]["audio"]
  assert base64.b64decode(audio["data"]) == chunks[0]
  assert audio["mimeType"] == "audio/pcm;rate=16000"

  # The interim's CJK spacing is normalized; the final ends the stream.
  assert [(event.kind, event.text) for event in events] == [("partial", "你好"), ("final", "final text")]


def test_setup_frame_maps_languages_and_drops_unknown_codes() -> None:
  backend = GeminiTranscriptionBackend(CharlieBotConfig())
  mapped = backend._setup_frame(["V"], ["zh", "fr", "en"])
  codes = mapped["setup"]["inputAudioTranscription"]["languageCodes"]
  assert codes == ["cmn-Hans-CN", "en-US"]
  empty = backend._setup_frame([], [])
  # An empty list is sent verbatim: it is the API's auto-detect signal.
  assert empty["setup"]["inputAudioTranscription"]["languageCodes"] == []
  assert empty["setup"]["inputAudioTranscription"]["customVocabulary"] == []


@pytest.mark.asyncio
async def test_interim_spaces_survive_next_to_latin_text(gemini_credentials) -> None:
  """Only spaces between two CJK characters are dropped: Latin neighbours keep theirs."""

  async def handler(socket) -> None:
    async for raw in socket:
      message = json.loads(raw)
      if "setup" in message:
        await socket.send(json.dumps({"setupComplete": {}}))
      elif "activityEnd" in message.get("realtimeInput", {}):
        await socket.send(json.dumps({"serverContent": {"interimInputTranscription": {"text": "hello 你 好 world"}}}))
        await socket.send(json.dumps({"serverContent": {"inputTranscription": {"text": "hello 你好 world"}}}))
        await socket.close()

  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    events = await _collect(_backend(port), [b"\x00\x00" * 2048], vocabulary=[], languages=[])

  assert [(event.kind, event.text) for event in events] == [
      ("partial", "hello 你好 world"),
      ("final", "hello 你好 world"),
  ]


@pytest.mark.asyncio
async def test_final_ends_the_stream_even_when_the_server_keeps_talking(gemini_credentials) -> None:
  """The generationComplete arriving with the final says no text follows: the backend
  returns instead of reading the late interim."""

  async def handler(socket) -> None:
    async for raw in socket:
      message = json.loads(raw)
      if "setup" in message:
        await socket.send(json.dumps({"setupComplete": {}}))
      elif "activityEnd" in message.get("realtimeInput", {}):
        await socket.send(
            json.dumps({"serverContent": {
                "inputTranscription": {
                    "text": "done"
                },
                "generationComplete": True,
            }}))
        await socket.send(json.dumps({"serverContent": {"interimInputTranscription": {"text": "late"}}}))
        await socket.close()

  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    events = await _collect(_backend(port), [b"\x00\x00" * 2048], vocabulary=[], languages=[])

  assert [(event.kind, event.text) for event in events] == [("final", "done")]


@pytest.mark.asyncio
async def test_non_setup_complete_reply_rejects(gemini_credentials) -> None:

  async def handler(socket) -> None:
    async for _raw in socket:
      await socket.send(json.dumps({"error": {"code": 400, "message": "bad setup"}}))
      await socket.close()
      return

  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    with pytest.raises(TranscriptionRejected, match="setupComplete"):
      await _collect(_backend(port), [b"\x00\x00" * 2048], vocabulary=[], languages=[])


@pytest.mark.asyncio
async def test_failing_feed_ends_the_session_instead_of_hanging(gemini_credentials) -> None:
  """An audio source that raises mid-recording must end the session promptly: the
  dead sender closes the socket and the receive loop surfaces an error."""

  async def handler(socket) -> None:
    async for raw in socket:
      if "setup" in json.loads(raw):
        await socket.send(json.dumps({"setupComplete": {}}))
        async for _message in socket:
          pass

  async def broken_feed() -> AsyncIterator[bytes]:
    yield b"\x00\x00" * 2048
    raise RuntimeError("feed exploded")

  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    with pytest.raises((RuntimeError, ConnectionClosed)):
      await asyncio.wait_for(_collect(_backend(port), broken_feed(), vocabulary=[], languages=[]), 10)


@pytest.mark.asyncio
async def test_mid_stream_close_raises(gemini_credentials) -> None:

  async def handler(socket) -> None:
    async for raw in socket:
      message = json.loads(raw)
      if "setup" in message:
        await socket.send(json.dumps({"setupComplete": {}}))
      elif "audio" in message.get("realtimeInput", {}):
        await socket.close(code=1008, reason="gone")
        return

  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    with pytest.raises(ConnectionClosed):
      await _collect(_backend(port), [b"\x00\x00" * 2048, b"\x00\x00" * 2048], vocabulary=[], languages=[])
