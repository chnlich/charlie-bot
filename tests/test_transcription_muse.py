"""Muse transcription backend tests: the migrated fake-server cases plus the pacing schedule.

The client moved from scripts/voice_replay_eval.py into
src/agents/transcription/muse.py; these are its tests. No network beyond
127.0.0.1, and the pacing test runs on an injected virtual clock, so it sends a
whole backlog without waiting real time.
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import AsyncIterator
from pathlib import Path

import numpy as np
import pytest
from websockets.asyncio.server import serve

from src.agents.transcription.base import TranscriptEvent, TranscriptionRejected
from src.agents.transcription.muse import FRAME_SAMPLES, LEAD_CAP_S, MuseTranscriptionBackend
from src.core.config import CharlieBotConfig
from src.core.credentials import Credentials

SAMPLE_RATE = 16_000
FRAME_BYTES = FRAME_SAMPLES * 2

EXPECTED_HANDSHAKE = {
    "mode": "PUSH_TO_TALK",
    "authorization": {
        "accessToken": "test-token"
    },
    "audioEncoding": "PCM_16KHZ",
    "model": "muse-voice-transcribe-1.0",
    "partialMode": "CUMULATIVE",
    "emitAudioProgress": False,
}


@pytest.fixture
def muse_credentials(monkeypatch: pytest.MonkeyPatch) -> Credentials:
  """A credentials source with the meta key set; the value never reaches an assertion."""
  credentials = Credentials(path=Path("credentials.yaml"), sections={"meta": {"model_api_key": "test-token"}})
  monkeypatch.setattr("src.agents.transcription.muse.get_credentials", lambda: credentials)
  return credentials


def _serve(handler):
  return serve(handler, "127.0.0.1", 0)


def _backend(port: int, clock=None) -> MuseTranscriptionBackend:
  return MuseTranscriptionBackend(CharlieBotConfig(), endpoint_url=f"ws://127.0.0.1:{port}", clock=clock)


def _sine_pcm(seconds: float) -> bytes:
  positions = np.arange(int(SAMPLE_RATE * seconds), dtype=np.float64)
  return (np.sin(2 * np.pi * 440.0 * positions / SAMPLE_RATE) * 10_000).astype("<i2").tobytes()


def _frames(pcm: bytes) -> list[bytes]:
  return [pcm[offset:offset + FRAME_BYTES] for offset in range(0, len(pcm), FRAME_BYTES)]


async def _collect(backend: MuseTranscriptionBackend, feed: AsyncIterator[bytes], **kwargs) -> list[TranscriptEvent]:
  return [event async for event in backend.transcribe(feed, **kwargs)]


async def _feed_frames(chunks: list[bytes]) -> AsyncIterator[bytes]:
  for chunk in chunks:
    yield chunk


@pytest.mark.asyncio
async def test_handshake_frames_partials_and_final(muse_credentials) -> None:
  observed: dict = {}

  async def handler(socket) -> None:
    observed["handshake"] = json.loads(await socket.recv())
    await socket.send(json.dumps({"type": "ready"}))
    frames = []
    async for message in socket:
      frames.append(message)
      if isinstance(message, bytes):
        if len(frames) == 2:
          await socket.send(json.dumps({"type": "transcript", "text": "par one", "final": False}))
        elif len(frames) == 3:
          await socket.send(json.dumps({"type": "transcript", "text": "par one two", "final": False}))
      else:
        observed["end_stream"] = json.loads(message)
        await socket.send(json.dumps({"type": "transcript", "text": "hello world", "final": True}))
        await socket.send(json.dumps({"type": "speechComplete", "transcript": "hello world"}))
        await socket.close()
    observed["frames"] = frames

  chunks = _frames(_sine_pcm(3 * FRAME_SAMPLES / SAMPLE_RATE))
  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    events = await asyncio.wait_for(_collect(_backend(port), _feed_frames(chunks), vocabulary=[], languages=[]), 30)

  # Handshake is the first frame, with exactly the settled fields; keywords and
  # languageBias are absent when empty.
  assert observed["handshake"] == EXPECTED_HANDSHAKE
  audio = observed["frames"][:-1]
  assert all(isinstance(frame, bytes) for frame in audio)
  assert len(audio) == 3
  assert observed["end_stream"] == {"type": "endStream"}
  assert observed["frames"][-1] == json.dumps({"type": "endStream"})
  assert [(event.kind, event.text) for event in events] == [
      ("partial", "par one"),
      ("partial", "par one two"),
      ("final", "hello world"),
  ]


@pytest.mark.asyncio
async def test_keywords_and_mapped_language_bias_reach_the_handshake(muse_credentials) -> None:
  observed: dict = {}

  async def handler(socket) -> None:
    observed["handshake"] = json.loads(await socket.recv())
    await socket.send(json.dumps({"type": "ready"}))
    async for _message in socket:
      await socket.send(json.dumps({"type": "transcript", "text": "x", "final": True}))
      await socket.send(json.dumps({"type": "speechComplete", "transcript": "x"}))
      await socket.close()

  chunks = _frames(_sine_pcm(0.2))
  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    await asyncio.wait_for(
        _collect(_backend(port), _feed_frames(chunks), vocabulary=["CharlieBot"], languages=["zh", "fr", "en"]), 30)

  # The vocabulary rides as keywords; BCP-47 codes map to languageBias and the
  # unmappable one is dropped.
  assert observed["handshake"] == {
      **EXPECTED_HANDSHAKE,
      "keywords": ["CharlieBot"],
      "languageBias": ["Mandarin Chinese", "English"],
  }


@pytest.mark.asyncio
async def test_handshake_error_rejects(muse_credentials) -> None:

  async def handler(socket) -> None:
    await socket.recv()
    await socket.send(json.dumps({"type": "error", "message": "invalid credentials"}))
    await socket.close()

  chunks = _frames(_sine_pcm(0.2))
  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    with pytest.raises(TranscriptionRejected, match="invalid credentials"):
      await asyncio.wait_for(_collect(_backend(port), _feed_frames(chunks), vocabulary=[], languages=[]), 30)


@pytest.mark.asyncio
async def test_abnormal_close_mid_stream_raises_with_the_code(muse_credentials) -> None:

  async def handler(socket) -> None:
    await socket.recv()
    await socket.send(json.dumps({"type": "ready"}))
    async for _message in socket:
      await socket.close(code=1008, reason="audio backlog exceeded")

  chunks = _frames(_sine_pcm(0.4))
  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    with pytest.raises(RuntimeError, match=r"code 1008: audio backlog exceeded"):
      await asyncio.wait_for(_collect(_backend(port), _feed_frames(chunks), vocabulary=[], languages=[]), 30)


@pytest.mark.asyncio
async def test_final_transcript_without_speech_complete_still_yields_the_final(muse_credentials) -> None:
  """The socket closed after the final but before speechComplete: the last final
  transcript is the recording's text."""

  async def handler(socket) -> None:
    await socket.recv()
    await socket.send(json.dumps({"type": "ready"}))
    async for _message in socket:
      await socket.send(json.dumps({"type": "transcript", "text": "just final", "final": True}))
      await socket.close()

  chunks = _frames(_sine_pcm(0.2))
  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    events = await asyncio.wait_for(_collect(_backend(port), _feed_frames(chunks), vocabulary=[], languages=[]), 30)

  assert [(event.kind, event.text) for event in events] == [("final", "just final")]


@pytest.mark.asyncio
async def test_unreadable_event_surfaces_instead_of_hanging(muse_credentials) -> None:
  """A server message the parser cannot read ends the session with the error, not a
  silent wait for events that will never come."""

  async def handler(socket) -> None:
    await socket.recv()
    await socket.send(json.dumps({"type": "ready"}))
    await socket.send("this is not json")
    async for _message in socket:
      pass

  chunks = _frames(_sine_pcm(0.2))
  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    with pytest.raises(json.JSONDecodeError):
      await asyncio.wait_for(_collect(_backend(port), _feed_frames(chunks), vocabulary=[], languages=[]), 10)


@pytest.mark.asyncio
async def test_failing_feed_surfaces_instead_of_hanging(muse_credentials) -> None:
  """An audio source that raises mid-recording ends the session with that error."""

  async def handler(socket) -> None:
    await socket.recv()
    await socket.send(json.dumps({"type": "ready"}))
    async for _message in socket:
      pass

  async def broken_feed() -> AsyncIterator[bytes]:
    yield _frames(_sine_pcm(0.2))[0]
    raise RuntimeError("feed exploded")

  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    with pytest.raises(RuntimeError, match="feed exploded"):
      await asyncio.wait_for(_collect(_backend(port), broken_feed(), vocabulary=[], languages=[]), 10)


class _VirtualClock:
  """Injected pacing clock: only sleep advances it, so recorded times are the
  schedule's own times and the test waits no real time."""

  def __init__(self) -> None:
    self._now = 100.0

  def monotonic(self) -> float:
    return self._now

  async def sleep(self, seconds: float) -> None:
    self._now += seconds


@pytest.mark.asyncio
async def test_pacing_caps_the_lead_at_four_seconds_and_drains_the_backlog(muse_credentials) -> None:
  """Every chunk pre-queued before the handshake: the flush sends at once up to the
  4 s lead (one frame of send quantization), the tail drains at real time, and the
  lead never grows past the cap."""
  clock = _VirtualClock()
  received: list[tuple[float, float]] = []  # (clock time at recv, chunk seconds)
  ready_at: list[float] = []
  total_s = LEAD_CAP_S + 2.0  # a 4 s flush plus a 2 s paced tail
  frame_s = FRAME_SAMPLES / SAMPLE_RATE

  async def handler(socket) -> None:
    await socket.recv()
    ready_at.append(clock.monotonic())
    await socket.send(json.dumps({"type": "ready"}))
    async for message in socket:
      if isinstance(message, bytes):
        received.append((clock.monotonic(), len(message) / 2 / SAMPLE_RATE))
      else:
        await socket.send(json.dumps({"type": "transcript", "text": "done", "final": True}))
        await socket.send(json.dumps({"type": "speechComplete", "transcript": "done"}))
        await socket.close()

  pcm = _sine_pcm(total_s)

  async def backlog() -> AsyncIterator[bytes]:
    for chunk in _frames(pcm):
      yield chunk

  async with _serve(handler) as server:
    port = server.sockets[0].getsockname()[1]
    events = await asyncio.wait_for(_collect(_backend(port, clock), backlog(), vocabulary=[], languages=[]), 30)

  assert [event.kind for event in events] == ["final"]
  assert ready_at and received
  session_start = ready_at[0]
  # The whole pre-queued backlog reaches the server...
  assert sum(seconds for _, seconds in received) == pytest.approx(total_s)
  # ...no send leads the session clock by more than the cap plus one frame of
  # quantization (a send cannot be smaller than a frame)...
  cumulative = 0.0
  for at, seconds in received:
    cumulative += seconds
    assert cumulative - (at - session_start) <= LEAD_CAP_S + frame_s + 1e-6
  # ...and the tail drains at the capped pace: a no-flush schedule (1x from the
  # session start) would need total_s, the cap lets it finish total_s - LEAD_CAP_S.
  assert received[-1][0] - session_start <= total_s - LEAD_CAP_S + frame_s + 1e-6
