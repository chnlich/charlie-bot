"""The Muse Voice Transcribe backend's close wait.

The cancel of a transcription in progress exits `connect(...)`, whose close
handshake waits close_timeout for the peer's close frame. The shared conftest
probe drives that cancel against a stand-in that never answers one; the pin
keeps that wait at `WS_CLIENT_CLOSE_TIMEOUT`, since an unpinned connect() rides
websockets' 10 s stop default on every stop that has a transcription open.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import assert_transcribe_cancel_honors_close_timeout

from src.agents.transcription import muse
from src.agents.transcription.muse import MuseTranscriptionBackend
from src.core.config import CharlieBotConfig
from src.core.credentials import Credentials

# The handshake reply the backend accepts: any JSON without an "error" key.
_HANDSHAKE_REPLY = [{"type": "sessionStarted"}]


@pytest.mark.asyncio
async def test_transcribe_cancel_waits_only_the_configured_close_timeout(monkeypatch: pytest.MonkeyPatch) -> None:
  """Cancelling a transcription in progress waits WS_CLIENT_CLOSE_TIMEOUT for the
  peer's close frame, not websockets' 10 s default (synthetic credentials only)."""
  from src.core import timeouts

  monkeypatch.setattr(timeouts, "WS_CLIENT_CLOSE_TIMEOUT", 0.2)
  monkeypatch.setattr(
      muse,
      "get_credentials",
      lambda: Credentials(path=Path("/tmp/fake-credentials.yaml"), sections={"meta": {
          "model_api_key": "test-key"
      }}),
  )

  def backend(url: str) -> MuseTranscriptionBackend:
    return MuseTranscriptionBackend(CharlieBotConfig(charliebot_home=Path("/tmp/fake-home")), endpoint_url=url)

  await assert_transcribe_cancel_honors_close_timeout(backend, _HANDSHAKE_REPLY)
