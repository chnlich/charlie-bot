"""The websocket transcription backends' cancel close wait, one case per backend.

The cancel of a transcription in progress exits `connect(...)`, whose close
handshake waits close_timeout for the peer's close frame. The shared conftest
probe drives that cancel against a stand-in that never answers one; the pin
keeps that wait at `WS_CLIENT_CLOSE_TIMEOUT`, since an unpinned connect() rides
websockets' 10 s stop default on every stop that has a transcription open. The
case table carries the only per-backend differences: the module whose
credentials seam the test patches, the backend class, the credentials section
shape, and the handshake reply the backend accepts. Every credential here is
synthetic.
"""

from __future__ import annotations

from pathlib import Path
from types import ModuleType
from typing import Any

import pytest
from conftest import assert_transcribe_cancel_honors_close_timeout

from src.agents.transcription import muse
from src.agents.transcription.gemini import GeminiTranscriptionBackend
from src.agents.transcription.muse import MuseTranscriptionBackend
from src.core import credentials
from src.core.config import CharlieBotConfig
from src.core.credentials import Credentials

# The credentials section shape and the handshake reply are each backend's own
# wire contract; the sections dict is what the patched get_credentials() serves.
_GEMINI_CASE = (credentials, GeminiTranscriptionBackend, {"gemini": {"api_key": "test-key"}}, [{"setupComplete": {}}])
_MUSE_CASE = (muse, MuseTranscriptionBackend, {"meta": {"model_api_key": "test-key"}}, [{"type": "sessionStarted"}])

CLOSE_WAIT_CASES = [
    pytest.param(*_GEMINI_CASE, id="gemini"),
    pytest.param(*_MUSE_CASE, id="muse"),
]


@pytest.mark.asyncio
@pytest.mark.parametrize("mod, backend_cls, sections, handshake_reply", CLOSE_WAIT_CASES)
async def test_transcribe_cancel_waits_only_the_configured_close_timeout(
    monkeypatch: pytest.MonkeyPatch, mod: ModuleType, backend_cls: type[Any], sections: dict,
    handshake_reply: list[dict]) -> None:
  """Cancelling a transcription in progress waits WS_CLIENT_CLOSE_TIMEOUT for the
  peer's close frame, not websockets' 10 s default (synthetic credentials only)."""
  from src.core import timeouts

  monkeypatch.setattr(timeouts, "WS_CLIENT_CLOSE_TIMEOUT", 0.2)
  monkeypatch.setattr(
      mod,
      "get_credentials",
      lambda: Credentials(path=Path("/tmp/fake-credentials.yaml"), sections=sections),
  )

  def backend(url: str) -> Any:
    return backend_cls(CharlieBotConfig(charliebot_home=Path("/tmp/fake-home")), endpoint_url=url)

  await assert_transcribe_cancel_honors_close_timeout(backend, handshake_reply)
