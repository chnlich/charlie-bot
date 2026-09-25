"""Muse Voice Transcribe backend: Meta's realtime ASR over a PUSH_TO_TALK session.

The client moved here from scripts/voice_replay_eval.py (handshake, transcript
parsing, close-code handling) so exactly one implementation exists. Meta
disconnects when received audio leads real time by more than 5 s, so audio goes
out on an absolute schedule that leads the session clock by at most 4 s: chunks
queued before the handshake completed are flushed at once up to that lead, the
rest at real time (the LiteLLM Meta-realtime pacing pattern).
"""

from __future__ import annotations

import asyncio
import json
import time
from collections.abc import AsyncIterator, Sequence
from typing import Protocol

from src.agents.transcription.base import TranscriptEvent, TranscriptionBackend, TranscriptionRejected
from src.core.config import CharlieBotConfig
from src.core.credentials import get_credentials

MODEL = "muse-voice-transcribe-1.0"
SAMPLE_RATE = 16_000
BYTES_PER_SECOND = SAMPLE_RATE * 2  # mono PCM16
DEFAULT_ENDPOINT_URL = "wss://api.meta.ai/v1/asr/realtime"
# Meta's disconnect sits at a 5 s lead; the schedule caps the lead one second
# below it, so a slow handshake (D s) still lands the final inside the 2 s
# display budget while D stays under about 5.5 s.
LEAD_CAP_S = 4.0

# BCP-47 base codes -> Muse languageBias. Unmapped codes are dropped; an empty
# list sends no languageBias at all.
LANGUAGE_BIAS = {"zh": "Mandarin Chinese", "en": "English"}


class Clock(Protocol):
  """The pacing clock. Tests inject a virtual one; production uses wall time."""

  def monotonic(self) -> float:
    ...

  async def sleep(self, seconds: float) -> None:
    ...


class _EventLoopClock:
  """The production clock: monotonic wall time and asyncio sleep."""

  def monotonic(self) -> float:
    return time.monotonic()

  async def sleep(self, seconds: float) -> None:
    await asyncio.sleep(seconds)


class MuseTranscriptionBackend(TranscriptionBackend):
  id = "muse"
  label = "Muse Voice Transcribe"
  live_partials = True

  def __init__(
      self, cfg: CharlieBotConfig, endpoint_url: str = DEFAULT_ENDPOINT_URL, clock: Clock | None = None) -> None:
    self._cfg = cfg
    # Constructor argument, not config: tests point it at a loopback fake server.
    self._endpoint_url = endpoint_url
    self._clock: Clock = clock if clock is not None else _EventLoopClock()

  def unavailable_reason(self) -> str | None:
    if not get_credentials().get("meta", "model_api_key"):
      return "needs meta.model_api_key"
    return None

  async def transcribe(
      self,
      audio: AsyncIterator[bytes],
      *,
      vocabulary: Sequence[str],
      languages: Sequence[str],
  ) -> AsyncIterator[TranscriptEvent]:
    access_token = str(get_credentials().get("meta", "model_api_key") or "")
    if not access_token:
      raise TranscriptionRejected("meta.model_api_key is not set in credentials.yaml")
    from websockets.asyncio.client import connect
    from websockets.exceptions import ConnectionClosed

    async with connect(self._endpoint_url) as socket:
      # Never logs or returns the handshake, so the access token cannot reach
      # any output.
      await socket.send(json.dumps(self._handshake(access_token, vocabulary, languages)))
      try:
        reply = json.loads(await socket.recv())
      except (json.JSONDecodeError, TypeError) as exc:
        raise TranscriptionRejected(f"handshake reply is not JSON: {exc}") from exc
      if reply.get("type") == "error" or "error" in reply:
        raise TranscriptionRejected(f"handshake rejected: {reply.get('message', reply)}")

      # Reader and sender run concurrently: partials must reach the caller while
      # the audio is still going out. Every event (and the close) rides one
      # queue so the two failure paths meet in one loop.
      events: asyncio.Queue = asyncio.Queue()

      async def read_events() -> None:
        try:
          async for message in socket:
            events.put_nowait(json.loads(message))
        except ConnectionClosed as exc:
          # The server's close (clean or not) ends the event stream; the main
          # loop turns an abnormal one into an error unless a final arrived.
          events.put_nowait(exc)
        except Exception as exc:  # re-raised by the main loop, never swallowed
          events.put_nowait(exc)
        events.put_nowait(None)

      def on_sender_done(sender_task: asyncio.Task) -> None:
        """A sender that died must end the session now: the main loop would
        otherwise wait forever for events that will never come."""
        if sender_task.cancelled():
          return
        error = sender_task.exception()
        if error is not None:
          events.put_nowait(error)

      reader = asyncio.create_task(read_events())

      async def send_audio() -> None:
        # The session clock starts when the first chunk is sent (the handshake
        # has just completed and the queue is flushed); ``sent_s`` counts the
        # audio already sent. A chunk waits until the session clock reaches its
        # position minus the lead cap, so the lead never exceeds the cap and a
        # pre-handshake backlog goes out at once up to it.
        session_start: float | None = None
        sent_s = 0.0
        async for chunk in audio:
          if session_start is None:
            session_start = self._clock.monotonic()
          target = session_start + max(0.0, sent_s - LEAD_CAP_S)
          delay = target - self._clock.monotonic()
          if delay > 0:
            await self._clock.sleep(delay)
          await socket.send(chunk)
          sent_s += len(chunk) / BYTES_PER_SECOND
        await socket.send(json.dumps({"type": "endStream"}))

      sender = asyncio.create_task(send_audio())
      sender.add_done_callback(on_sender_done)
      pending_final: str | None = None
      try:
        while True:
          event = await events.get()
          if event is None:
            break
          if isinstance(event, ConnectionClosed):
            code = event.rcvd.code if event.rcvd is not None else None
            reason = event.rcvd.reason if event.rcvd is not None else ""
            raise RuntimeError(f"connection closed mid-stream (code {code}: {reason})")
          if isinstance(event, Exception):
            raise event
          kind = event.get("type")
          if kind == "transcript":
            if event.get("final"):
              pending_final = event.get("text", "")
            else:
              yield TranscriptEvent(kind="partial", text=event.get("text", ""))
          elif kind == "speechComplete":
            # speechComplete carries the authoritative cumulative text; the
            # final transcript event is the fallback.
            yield TranscriptEvent(kind="final", text=event.get("transcript") or pending_final or "")
            return
      finally:
        # Reap both tasks so their close exceptions are not lost to the GC.
        sender.cancel()
        reader.cancel()
        await asyncio.gather(sender, reader, return_exceptions=True)

      # The socket closed without speechComplete: the last final transcript is
      # the recording's text. Without one the stream produced nothing usable.
      if pending_final is None:
        raise RuntimeError("stream closed without a final transcript")
      yield TranscriptEvent(kind="final", text=pending_final)

  def _handshake(self, access_token: str, vocabulary: Sequence[str], languages: Sequence[str]) -> dict:
    """The first frame on the socket; keywords/languageBias only when non-empty."""
    handshake: dict = {
        "mode": "PUSH_TO_TALK",
        "authorization": {
            "accessToken": access_token
        },
        "audioEncoding": "PCM_16KHZ",
        "model": MODEL,
        "partialMode": "CUMULATIVE",
        "emitAudioProgress": False,
    }
    if vocabulary:
      handshake["keywords"] = list(vocabulary)
    bias = [LANGUAGE_BIAS[code] for code in languages if code in LANGUAGE_BIAS]
    if bias:
      handshake["languageBias"] = bias
    return handshake
