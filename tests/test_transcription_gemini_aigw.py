# -*- coding: utf-8 -*-
"""The gemini-aigw backend: one whole-clip POST through the gateway's /gemini pass-through.

Each test drives transcribe against a loopback fake gateway (a threaded
http.server recording every request and answering with the scripted reply) to
pin the request contract - path, key header, inline WAV, transcription config -
and the reply contract: the text comes from every part's
audioTranscription.text, joined into exactly one final. Every sentence here is
synthetic; the Chinese strings use escapes so the source stays ASCII.
"""

import base64
import contextlib
import io
import json
import threading
import wave
from collections.abc import AsyncIterator, Iterator
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import cast

import pytest

from src.agents.transcription.base import TranscriptionRejected
from src.agents.transcription.gemini_aigw import GeminiAigwTranscriptionBackend
from src.core.config import CharlieBotConfig
from src.core.credentials import Credentials

API_KEY = "test-aigw-key"
SAMPLES = 160  # 10 ms of PCM16
# Synthetic sentences; escapes keep this file ASCII.
FINAL_ZH = "\u4efb\u52a1\u5b8c\u6210\u4e86"
PARTS_WITH_SPACES = ("cron job \u7684 ", "session")
JOINED_WITH_SPACES = "cron job \u7684 session"


@dataclass
class _RecordedRequest:
  path: str
  headers: dict[str, str]
  body: bytes


class _FakeGatewayHandler(BaseHTTPRequestHandler):
  """Records one POST and answers with the gateway's scripted response."""

  def do_POST(self) -> None:
    gateway = cast(_FakeGateway, self.server)
    body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
    gateway.requests.append(
        _RecordedRequest(
            path=self.path,
            headers={
                key.lower(): value for key, value in self.headers.items()
            },
            body=body,
        ))
    status, payload = gateway.scripted
    self.send_response(status)
    self.send_header("Content-Type", "application/json")
    self.send_header("Content-Length", str(len(payload)))
    self.end_headers()
    self.wfile.write(payload)

  def log_message(self, format: str, *args: object) -> None:
    del format, args  # the tests read the recorded requests, not the console


class _FakeGateway(ThreadingHTTPServer):
  """The loopback gateway: recorded requests, one scripted response."""

  daemon_threads = True

  def __init__(self, status: int, payload: bytes) -> None:
    super().__init__(("127.0.0.1", 0), _FakeGatewayHandler)
    self.requests: list[_RecordedRequest] = []
    self.scripted = (status, payload)

  @property
  def base_url(self) -> str:
    host, port = self.server_address[:2]
    return f"http://{host}:{port}"


@contextlib.contextmanager
def _gateway(status: int, payload: dict | bytes) -> Iterator[_FakeGateway]:
  """Serve one scripted response on a loopback port for the duration of the block."""
  body = payload if isinstance(payload, bytes) else json.dumps(payload).encode("utf-8")
  server = _FakeGateway(status, body)
  threading.Thread(target=server.serve_forever, daemon=True).start()
  try:
    yield server
  finally:
    server.shutdown()
    server.server_close()


async def _one_chunk_audio() -> AsyncIterator[bytes]:
  yield b"\x01\x00" * SAMPLES


def _patch_credentials(monkeypatch: pytest.MonkeyPatch, api_key: str | None) -> None:
  sections = {} if api_key is None else {"aigw": {"api_key": api_key}}
  monkeypatch.setattr(
      "src.agents.transcription.gemini_aigw.get_credentials",
      lambda: Credentials(path=Path("/tmp/fake-credentials.yaml"), sections=sections))


def _backend(tmp_path: Path, base_url: str) -> GeminiAigwTranscriptionBackend:
  return GeminiAigwTranscriptionBackend(
      CharlieBotConfig(charliebot_home=tmp_path / "home", voice={"aigw_base_url": base_url}))


def _transcription_reply(*texts: str) -> dict:
  return {"candidates": [{"content": {"parts": [{"audioTranscription": {"text": text}} for text in texts]}}]}


def _decoded_inline_wav(data_b64: str) -> tuple[tuple[int, int, int], bytes]:
  """The inline data as (channels, sample width, rate) plus its PCM frames."""
  with wave.open(io.BytesIO(base64.b64decode(data_b64)), "rb") as reader:
    return (reader.getnchannels(), reader.getsampwidth(), reader.getframerate()), reader.readframes(reader.getnframes())


# --- The request contract ------------------------------------------------------


@pytest.mark.asyncio
async def test_the_whole_recording_goes_out_as_one_inline_wav_with_the_transcription_config(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  _patch_credentials(monkeypatch, API_KEY)
  with _gateway(200, _transcription_reply(FINAL_ZH)) as gateway:
    backend = _backend(tmp_path, gateway.base_url)

    events = [
        event async for event in backend.transcribe(
            _one_chunk_audio(), vocabulary=["CharlieBot", "sglang"], languages=["zh", "en", "fr"])
    ]

  assert len(gateway.requests) == 1
  request = gateway.requests[0]
  assert request.path == "/gemini/v1beta/models/gemini-3.5-transcribe:generateContent"
  assert request.headers["x-goog-api-key"] == API_KEY
  body = json.loads(request.body)
  (part,) = body["contents"][0]["parts"]
  assert part["inlineData"]["mimeType"] == "audio/wav"
  # The inline data is the PCM wrapped in the one accepted container: 16 kHz mono PCM16.
  (channels, width, rate), frames = _decoded_inline_wav(part["inlineData"]["data"])
  assert (channels, width, rate) == (1, 2, 16_000)
  assert frames == b"\x01\x00" * SAMPLES
  # The transcription config mirrors the Live setup: mapped codes only, the
  # vocabulary verbatim, VERBATIM mode.
  assert body["generationConfig"]["audioTranscriptionConfig"] == {
      "mode": "VERBATIM",
      "languageCodes": ["cmn-Hans-CN", "en-US"],
      "customVocabulary": ["CharlieBot", "sglang"],
  }
  assert [(event.kind, event.text) for event in events] == [("final", FINAL_ZH)]


@pytest.mark.asyncio
async def test_no_mapped_language_and_an_empty_vocabulary_send_no_fields(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  _patch_credentials(monkeypatch, API_KEY)
  with _gateway(200, _transcription_reply("hello")) as gateway:
    backend = _backend(tmp_path, gateway.base_url)

    events = [event async for event in backend.transcribe(_one_chunk_audio(), vocabulary=[], languages=["fr"])]

  config = json.loads(gateway.requests[0].body)["generationConfig"]["audioTranscriptionConfig"]
  assert config == {"mode": "VERBATIM"}
  assert [(event.kind, event.text) for event in events] == [("final", "hello")]


# --- The reply contract --------------------------------------------------------


@pytest.mark.asyncio
async def test_every_parts_text_joins_into_exactly_one_unnormalized_final(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  _patch_credentials(monkeypatch, API_KEY)
  # The HTTP reply carries no character-split spacing, and the backend applies
  # no normalizer: the parts join as they arrive, spaces included.
  with _gateway(200, _transcription_reply(*PARTS_WITH_SPACES)) as gateway:
    backend = _backend(tmp_path, gateway.base_url)

    events = [event async for event in backend.transcribe(_one_chunk_audio(), vocabulary=[], languages=["en"])]

  assert [(event.kind, event.text) for event in events] == [("final", JOINED_WITH_SPACES)]


@pytest.mark.asyncio
async def test_candidates_without_parts_join_into_one_empty_final(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  # Measured refusal shape: finishReason STOP with content {} — the model says
  # nothing, the same clips the Live engine loses to a 1008 policy close.
  _patch_credentials(monkeypatch, API_KEY)
  reply = {"candidates": [{"content": {}, "finishReason": "STOP", "index": 0}]}
  with _gateway(200, reply) as gateway:
    backend = _backend(tmp_path, gateway.base_url)

    events = [event async for event in backend.transcribe(_one_chunk_audio(), vocabulary=[], languages=[])]

  assert [(event.kind, event.text) for event in events] == [("final", "")]


@pytest.mark.asyncio
async def test_a_part_without_the_transcription_raises(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  # The 2026-09 parse bug's shape: a part whose text sits somewhere else must
  # not pass as a silent recording.
  _patch_credentials(monkeypatch, API_KEY)
  reply = {"candidates": [{"content": {"parts": [{"text": "misplaced"}]}}]}
  with _gateway(200, reply) as gateway:
    backend = _backend(tmp_path, gateway.base_url)

    with pytest.raises(RuntimeError) as excinfo:
      await anext(backend.transcribe(_one_chunk_audio(), vocabulary=[], languages=[]))

  assert "audioTranscription.text" in str(excinfo.value)


@pytest.mark.asyncio
async def test_a_401_is_a_rejection(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  _patch_credentials(monkeypatch, API_KEY)
  with _gateway(401, b'{"error": "unauthenticated"}') as gateway:
    backend = _backend(tmp_path, gateway.base_url)

    with pytest.raises(TranscriptionRejected) as excinfo:
      await anext(backend.transcribe(_one_chunk_audio(), vocabulary=[], languages=[]))

  assert "401" in str(excinfo.value)
  assert "unauthenticated" in str(excinfo.value)


@pytest.mark.asyncio
async def test_a_500s_message_carries_the_status_and_body_but_never_the_key(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  _patch_credentials(monkeypatch, API_KEY)
  with _gateway(500, b'{"error": {"message": "upstream exploded"}}') as gateway:
    backend = _backend(tmp_path, gateway.base_url)

    with pytest.raises(RuntimeError) as excinfo:
      await anext(backend.transcribe(_one_chunk_audio(), vocabulary=[], languages=[]))

  message = str(excinfo.value)
  assert "500" in message
  assert "upstream exploded" in message
  assert API_KEY not in message


# --- Availability --------------------------------------------------------------


def test_unavailable_reason_names_the_missing_piece(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  without_url = GeminiAigwTranscriptionBackend(CharlieBotConfig(charliebot_home=tmp_path / "home"))
  assert without_url.unavailable_reason() == "needs voice.aigw_base_url"

  _patch_credentials(monkeypatch, None)
  without_key = GeminiAigwTranscriptionBackend(
      CharlieBotConfig(charliebot_home=tmp_path / "home", voice={"aigw_base_url": "https://example.invalid"}))
  assert without_key.unavailable_reason() == "needs aigw.api_key"


def test_a_fully_configured_backend_is_available(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  _patch_credentials(monkeypatch, API_KEY)
  backend = GeminiAigwTranscriptionBackend(
      CharlieBotConfig(charliebot_home=tmp_path / "home", voice={"aigw_base_url": "https://example.invalid"}))
  assert backend.unavailable_reason() is None
