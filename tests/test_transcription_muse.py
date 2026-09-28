"""The Muse Voice Transcribe backend's close wait.

The cancel of a transcription in progress exits `connect(...)`, whose close
handshake waits close_timeout for the peer's close frame. The stand-in here
never answers one, so the wait used to be websockets' 10 s default on every
stop that had a transcription open.
"""

from __future__ import annotations

import asyncio
import contextlib
import time
from pathlib import Path
from typing import Self

import pytest
from conftest import WsServerNeverAnswersClose

from src.agents.transcription.muse import MuseTranscriptionBackend
from src.core.config import CharlieBotConfig
from src.core.credentials import Credentials

# The handshake reply the backend accepts: any JSON without an "error" key.
_HANDSHAKE_REPLY = [{"type": "sessionStarted"}]


class _PendingAudio:
  """An audio stream that never delivers a chunk: the transcription stays open."""

  def __aiter__(self) -> Self:
    return self

  async def __anext__(self) -> bytes:
    await asyncio.Event().wait()
    raise AssertionError("unreachable: the pending audio never yields")


@pytest.mark.asyncio
async def test_transcribe_cancel_waits_only_the_configured_close_timeout(monkeypatch: pytest.MonkeyPatch) -> None:
  """Cancelling a transcription in progress waits WS_CLIENT_CLOSE_TIMEOUT for the
  peer's close frame, not websockets' 10 s default (synthetic credentials only)."""
  from src.agents.transcription import muse
  from src.core import timeouts

  monkeypatch.setattr(timeouts, "WS_CLIENT_CLOSE_TIMEOUT", 0.2)
  stand_in = WsServerNeverAnswersClose(_HANDSHAKE_REPLY)
  url = await stand_in.start()
  monkeypatch.setattr(
      muse,
      "get_credentials",
      lambda: Credentials(path=Path("/tmp/fake-credentials.yaml"), sections={"meta": {
          "model_api_key": "test-key"
      }}),
  )
  backend = MuseTranscriptionBackend(CharlieBotConfig(charliebot_home=Path("/tmp/fake-home")), endpoint_url=url)

  async def consume() -> None:
    async for _event in backend.transcribe(_PendingAudio(), vocabulary=[], languages=["zh"]):
      pass

  task = asyncio.create_task(consume(), name="transcribe-under-test")
  try:
    async with asyncio.timeout(5):
      while stand_in.received_chunks < 1:
        await asyncio.sleep(0.02)
    # The handshake reply was already on the wire when the stand-in saw the
    # client's handshake frame, so by now it is consumed and the transcription
    # is in progress.
    await asyncio.sleep(0.1)
    started = time.perf_counter()
    task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
      await task
    elapsed = time.perf_counter() - started
    assert elapsed < timeouts.WS_CLIENT_CLOSE_TIMEOUT + 0.5, (
        f"transcription cancel took {elapsed:.3f}s; the close wait did not honor "
        f"WS_CLIENT_CLOSE_TIMEOUT={timeouts.WS_CLIENT_CLOSE_TIMEOUT}")
  finally:
    task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
      await task
    await stand_in.stop()
